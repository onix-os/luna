//! String-library behaviours that used to disagree with PUC-Lua 5.4: `gsub` around empty matches,
//! replacement capture indices, the C character classes, `string.format` coercion and `%q`
//! literals, and `string.unpack`'s bounds checks.

use luna::{Closure, Executor, ExternError, Lua};

fn eval<T: for<'gc> luna::FromMultiValue<'gc> + 'static>(source: &str) -> Result<T, ExternError> {
    let mut lua = Lua::full();
    let executor = lua.try_enter(|ctx| {
        let closure = Closure::load(ctx, Some("probe"), source.as_bytes())?;
        Ok(ctx.stash(Executor::start(ctx, closure.into(), ())))
    })?;
    lua.execute::<T>(&executor)
}

/// Run `expr`, which must yield a string and a count, and render it as `"result count"`.
fn subst(expr: &str) -> String {
    eval::<String>(&format!(
        "local s, n = {expr} return tostring(s) .. ' ' .. tostring(n)"
    ))
    .unwrap()
}

fn text(expr: &str) -> String {
    eval::<String>(&format!("return tostring({expr})")).unwrap()
}

/// Run `expr` under `pcall`, returning the error message, or `"ok: <value>"` when it succeeds.
fn attempt(expr: &str) -> String {
    eval::<String>(&format!(
        "local ok, e = pcall(function() return {expr} end)
         if ok then return 'ok: ' .. tostring(e) else return tostring(e) end"
    ))
    .unwrap()
}

// 1. gsub must copy the characters it steps over between empty matches.

#[test]
fn gsub_keeps_the_text_between_empty_matches() {
    // A pattern that can match nothing matches once at every position, and the source character
    // in between has to survive. Dropping it silently deleted the whole subject.
    assert_eq!(subst(r#"("hello"):gsub("x*", "-")"#), "-h-e-l-l-o- 6");
    assert_eq!(subst(r#"("abc"):gsub("", ".")"#), ".a.b.c. 4");
    assert_eq!(subst(r#"("hello"):gsub("l*", "-")"#), "-h-e-o- 4");
    assert_eq!(subst(r#"("abc"):gsub("b*", "-")"#), "-a-c- 3");
    assert_eq!(subst(r#"("aaa"):gsub("a*", "-")"#), "- 1");
    assert_eq!(subst(r#"("abc"):gsub("$", "!")"#), "abc! 1");
}

#[test]
fn gsub_honours_the_substitution_limit_around_empty_matches() {
    assert_eq!(subst(r#"("hello"):gsub("", ".", 2)"#), ".h.ello 2");
    assert_eq!(subst(r#"("hello"):gsub("l", "L", 1)"#), "heLlo 1");
    assert_eq!(subst(r#"("hello"):gsub("x*", "-", 0)"#), "hello 0");
}

#[test]
fn gsub_table_and_function_replacements_agree_with_the_string_form() {
    assert_eq!(subst(r#"("abc"):gsub("x*", {})"#), "abc 4");
    assert_eq!(
        subst(r#"("abc"):gsub("", function() return "|" end)"#),
        "|a|b|c| 4"
    );
    assert_eq!(
        subst(r#"("hello"):gsub("x*", function() return "-" end)"#),
        "-h-e-l-l-o- 6"
    );
    assert_eq!(subst(r#"("abc"):gsub(".", {a = "A", b = false})"#), "Abc 3");
}

#[test]
fn an_anchored_gsub_still_substitutes_at_most_once() {
    assert_eq!(subst(r#"("aaa"):gsub("^a", "X")"#), "Xaa 1");
    assert_eq!(
        subst(r#"("aaa"):gsub("^a", function() return "Y" end)"#),
        "Yaa 1"
    );
    assert_eq!(subst(r#"("aaa"):gsub("^(a)", { a = "Z" })"#), "Zaa 1");
    assert_eq!(subst(r#"("abc"):gsub("^x*", "-")"#), "-abc 1");
}

#[test]
fn gmatch_and_find_report_empty_matches_the_same_way() {
    assert_eq!(
        eval::<String>(
            r#"
            local out = {}
            for w in ("hello"):gmatch("x*") do out[#out+1] = "[" .. w .. "]" end
            return table.concat(out)
        "#
        )
        .unwrap(),
        "[][][][][][]"
    );
    assert_eq!(
        eval::<String>(
            r#"
            local out = {}
            for w in ("abc"):gmatch("b*") do out[#out+1] = "[" .. w .. "]" end
            return table.concat(out)
        "#
        )
        .unwrap(),
        "[][b][]"
    );
    assert_eq!(
        eval::<String>(r#"return table.concat({("abc"):find("")}, ",")"#).unwrap(),
        "1,0"
    );
    assert_eq!(
        eval::<String>(r#"return table.concat({("abc"):find("x*")}, ",")"#).unwrap(),
        "1,0"
    );
}

// 2. `%1` with no captures in the pattern.

#[test]
fn a_replacement_capture_index_of_one_falls_back_to_the_whole_match() {
    assert_eq!(subst(r#"("abc"):gsub("b", "[%1]")"#), "a[b]c 1");
    assert_eq!(subst(r#"("abc"):gsub("b", "[%0]")"#), "a[b]c 1");
    assert_eq!(subst(r#"("abc"):gsub("(b)", "[%1]")"#), "a[b]c 1");
    assert_eq!(
        subst(r#"("hello world"):gsub("(%w+) (%w+)", "%2 %1")"#),
        "world hello 1"
    );
}

#[test]
fn a_replacement_capture_index_above_the_capture_count_is_still_an_error() {
    assert!(attempt(r#"("abc"):gsub("b", "[%2]")"#).contains("invalid capture index %2"));
    assert!(attempt(r#"("abc"):gsub("(b)", "[%2]")"#).contains("invalid capture index %2"));
    assert!(attempt(r#"("abc"):gsub("(b)", "[%3]")"#).contains("invalid capture index %3"));
}

// 3. Character classes follow C's ctype in the C locale.

#[test]
fn the_space_class_covers_every_byte_c_calls_whitespace() {
    // Rust's `is_ascii_whitespace` leaves out the vertical tab; C's `isspace` does not.
    for escape in [r"\v", r"\t", r"\n", r"\f", r"\r", " "] {
        assert_eq!(
            text(&format!(r#"("a{escape}b"):match("a%sb") ~= nil"#)),
            "true",
            "%s should match {escape}"
        );
    }
    assert_eq!(text(r#"("\v"):match("%S")"#), "nil");
    assert_eq!(text(r#"("x"):match("%s")"#), "nil");
}

#[test]
fn the_other_classes_match_their_ctype_definitions() {
    let checks = [
        (r#"("\127"):match("%c") ~= nil"#, "true"),
        (r#"("\31"):match("%c") ~= nil"#, "true"),
        (r#"(" "):match("%c")"#, "nil"),
        (r#"("~"):match("%g") ~= nil"#, "true"),
        (r#"(" "):match("%g")"#, "nil"),
        (r#"("_"):match("%p") ~= nil"#, "true"),
        (r#"("a"):match("%p")"#, "nil"),
        (r#"("F"):match("%x") ~= nil"#, "true"),
        (r#"("g"):match("%x")"#, "nil"),
        (r#"("_"):match("%w")"#, "nil"),
        (r#"("9"):match("%w") ~= nil"#, "true"),
        // Bytes above 127 are outside every class in the C locale, so only the negations match.
        (r#"("\200"):match("%a")"#, "nil"),
        (r#"("\200"):match("%A") ~= nil"#, "true"),
        (r#"("\200"):match("%w")"#, "nil"),
        // 5.4 dropped `%z`, so it is an ordinary escaped letter again.
        (r#"("z"):match("%z") ~= nil"#, "true"),
        (r#"("\0"):match("%z")"#, "nil"),
    ];
    for (expr, want) in checks {
        assert_eq!(text(expr), want, "{expr}");
    }
}

// 4. Numeric specifiers coerce numeric strings.

#[test]
fn the_numeric_format_specifiers_accept_numeric_strings() {
    assert_eq!(text(r#"string.format("%d", "42")"#), "42");
    assert_eq!(text(r#"string.format("%d", "  42  ")"#), "42");
    assert_eq!(text(r#"string.format("%d", "0x10")"#), "16");
    assert_eq!(text(r#"string.format("%x", "255")"#), "ff");
    assert_eq!(text(r#"string.format("%f", "1.5")"#), "1.500000");
    assert_eq!(text(r#"string.format("%g", "1e3")"#), "1000");
    assert_eq!(text(r#"string.format("%d", "3.0")"#), "3");
}

#[test]
fn the_numeric_format_specifiers_still_reject_what_lua_rejects() {
    assert!(attempt(r#"string.format("%d", "abc")"#).contains("number expected, got string"));
    assert!(attempt(r#"string.format("%f", "abc")"#).contains("number expected, got string"));
    assert!(attempt(r#"string.format("%d", nil)"#).contains("number expected, got nil"));
    assert!(attempt(r#"string.format("%d", true)"#).contains("number expected, got boolean"));
    for arg in ["3.5", "\"3.5\"", "1e300", "0/0", "math.huge"] {
        assert!(
            attempt(&format!(r#"string.format("%d", {arg})"#))
                .contains("number has no integer representation"),
            "%d on {arg}"
        );
    }
}

// 5. `%q` writes literals that read back as the same value *and* the same subtype.

#[test]
fn quoted_floats_are_hex_literals_so_they_stay_floats() {
    assert_eq!(text(r#"string.format("%q", 2.0)"#), "0x1p+1");
    assert_eq!(text(r#"string.format("%q", 0.1)"#), "0x1.999999999999ap-4");
    assert_eq!(
        text(r#"string.format("%q", math.pi)"#),
        "0x1.921fb54442d18p+1"
    );
    assert_eq!(text(r#"string.format("%q", 0.0)"#), "0x0p+0");
    assert_eq!(text(r#"string.format("%q", -0.0)"#), "-0x0p+0");
    assert_eq!(text(r#"string.format("%q", math.huge)"#), "1e9999");
    assert_eq!(text(r#"string.format("%q", -math.huge)"#), "-1e9999");
    assert_eq!(text(r#"string.format("%q", 0/0)"#), "(0/0)");
}

#[test]
fn quoted_values_round_trip_through_load_with_their_subtype() {
    assert_eq!(
        eval::<String>(
            r#"
            local function rt(v)
                local nv = load("return " .. string.format("%q", v))()
                return v == nv and math.type(v) == math.type(nv)
            end
            local ok = rt(2.0) and rt(0.1) and rt(math.pi) and rt(-0.0) and rt(0.0)
                and rt(math.huge) and rt(-math.huge) and rt(math.maxinteger) and rt(7)
                and rt("a\nb\0\1\255") and rt("he said \"hi\" \\")
            return tostring(ok)
        "#
        )
        .unwrap(),
        "true"
    );
}

#[test]
fn quoted_integers_use_the_hex_corner_case_for_mininteger() {
    assert_eq!(text(r#"string.format("%q", 7)"#), "7");
    assert_eq!(
        text(r#"string.format("%q", math.maxinteger)"#),
        "9223372036854775807"
    );
    // PUC writes the hex literal because the decimal form overflows to a float on the way back in.
    // luna's lexer does not yet wrap out-of-range hex integers, so this literal reads back as a
    // float here; the literal itself is what PUC produces.
    assert_eq!(
        text(r#"string.format("%q", math.mininteger)"#),
        "0x8000000000000000"
    );
}

#[test]
fn quoted_strings_escape_control_characters_the_way_puc_does() {
    // A newline becomes a backslash followed by a real newline; every other control byte becomes a
    // decimal escape, padded to three digits only when a digit follows it.
    assert_eq!(text(r#"string.format("%q", "a\nb")"#), "\"a\\\nb\"");
    assert_eq!(text(r#"string.format("%q", "a\rb")"#), "\"a\\13b\"");
    assert_eq!(text(r#"string.format("%q", "\0")"#), "\"\\0\"");
    assert_eq!(text(r#"string.format("%q", "\0" .. "1")"#), "\"\\0001\"");
    assert_eq!(text(r#"string.format("%q", "a\\b")"#), "\"a\\\\b\"");
    assert_eq!(text(r#"string.format("%q", "a\127b")"#), "\"a\\127b\"");
}

// 6. `string.unpack` raises Lua errors instead of panicking on out-of-range input.

#[test]
fn unpack_rejects_a_position_past_the_end_of_the_data() {
    // Every one of these sliced out of bounds and panicked the host before the bounds checks.
    for call in [
        r#"string.unpack("z", "abc", 10)"#,
        r#"string.unpack("i4", "ab", 99)"#,
        r#"string.unpack("B", "abc", math.maxinteger)"#,
        r#"string.unpack("z", "abc", 5)"#,
    ] {
        let message = attempt(call);
        assert!(
            message.contains("out of string") || message.contains("data string too short"),
            "{call} gave {message}"
        );
    }
}

#[test]
fn unpack_rejects_a_length_prefix_that_overruns_the_data() {
    // An `s8` length field of 2^64-1 overflowed the offset arithmetic outright.
    assert!(
        attempt(r#"string.unpack("<s8", "\255\255\255\255\255\255\255\255")"#)
            .contains("data string too short")
    );
    assert!(attempt(r#"string.unpack("s1", "\009abc")"#).contains("data string too short"));
    assert!(attempt(r#"string.unpack("c3", "ab")"#).contains("data string too short"));
    assert!(attempt(r#"string.unpack("xz", "")"#).contains("data string too short"));
    assert!(attempt(r#"string.unpack("x", "")"#).contains("data string too short"));
    assert!(attempt(r#"string.unpack("z", "abc")"#).contains("unfinished string"));
}

#[test]
fn unpack_reads_negative_and_zero_positions_relative_to_the_end() {
    assert_eq!(
        eval::<String>(
            r#"
            local data = string.pack("<i4i4", 7, 9)
            local a = string.unpack("<i4", data, -4)
            local b = string.unpack("<i4", data, 0)
            local c = string.unpack("<i4", data, -100)
            return a .. " " .. b .. " " .. c
        "#
        )
        .unwrap(),
        "9 7 7"
    );
}

#[test]
fn unpack_survives_random_formats_and_positions() {
    // A panic here aborts the process rather than failing an assertion, which is the point.
    eval::<()>(
        r#"
        local opts = {"b","B","h","H","i","I","l","L","j","J","T","f","d","n","s","z","c","x","X",
                      "<",">","=","!","i1","i8","i16","s1","s8","s16","c1","c16","!8"," "}
        local data = {"", "a", "abcd", "abcdefgh", string.rep("\255", 20), string.rep("\0", 20),
                      "\255\255\255\255\255\255\255\255abc", "\1\0\0\0\0\0\0\0abc"}
        local pos = {nil, 0, 1, 3, -1, -100, 99, math.maxinteger, math.mininteger}
        local seed = 4711
        local function rnd(n) seed = (seed * 1103515245 + 12345) % 2147483648 return seed % n + 1 end
        for _ = 1, 20000 do
            local f = {}
            for _ = 1, rnd(4) do f[#f+1] = opts[rnd(#opts)] end
            local fmt = table.concat(f)
            pcall(string.unpack, fmt, data[rnd(#data)], pos[rnd(#pos)])
            pcall(string.packsize, fmt)
            pcall(string.pack, fmt, "ab", 1, 2.5, "cd", 3)
        end
    "#,
    )
    .unwrap();
}

#[test]
fn pattern_matching_survives_random_patterns() {
    eval::<()>(
        r#"
        local atoms = {"a","b","%a","%s","%d","%w","%p","%c","%x","%A",".","*","+","-","?","^","$",
                       "(",")","[","]","%b()","%f[%a]","%1","%2","[a-c]","[^a]","%%"}
        local subjects = {"", "a", "abc", "hello world", "aaa", "a\vb\tc", string.rep("ab", 20)}
        local repls = {"-", "%0", "%1", "[%1]", "%%"}
        local seed = 991
        local function rnd(n) seed = (seed * 1103515245 + 12345) % 2147483648 return seed % n + 1 end
        for _ = 1, 15000 do
            local p = {}
            for _ = 1, rnd(4) do p[#p+1] = atoms[rnd(#atoms)] end
            local pat, s = table.concat(p), subjects[rnd(#subjects)]
            pcall(string.gsub, s, pat, repls[rnd(#repls)])
            pcall(string.gsub, s, pat, function(x) return x end)
            pcall(string.find, s, pat)
            pcall(string.match, s, pat)
            pcall(function()
                local it = string.gmatch(s, pat)
                for _ = 1, 100 do if it() == nil then break end end
            end)
        end
    "#,
    )
    .unwrap();
}

// 7. `cN` is the one variable-width option, bounded only by luna's string length cap.

#[test]
fn a_fixed_width_string_is_not_capped_at_sixteen_bytes() {
    // Only the integer-shaped options have a real width limit; `c` used to borrow theirs and
    // reject anything past `c16`.
    assert_eq!(text(r#"#string.pack("c20", "hi")"#), "20");
    assert_eq!(
        text(r#"string.pack("c20", "hi") == "hi" .. string.rep("\0", 18)"#),
        "true"
    );
    assert_eq!(text(r#"string.packsize("c100")"#), "100");
    assert_eq!(
        text(r#"string.unpack("c20", string.pack("c20", "hi"))"#),
        "hi\0".to_owned() + &"\0".repeat(17)
    );
    assert_eq!(text(r#"#string.pack("c0", "")"#), "0");
    assert_eq!(text(r#"string.packsize("<c7")"#), "7");
    // A string that does not fit the declared width is still an error.
    assert!(attempt(r#"string.pack("c1", "hi")"#).contains("string longer than given size"));
    assert!(attempt(r#"string.pack("c", "x")"#).contains("missing size for format option 'c'"));
}

#[test]
fn a_fixed_width_string_past_the_string_cap_is_rejected() {
    // The cap is 1 GiB. A `c` at the cap is fine; one byte past it, an absurd count, and a format
    // that reaches the cap by repetition all have to be refused before anything is allocated.
    assert_eq!(text(r#"string.packsize("c1073741824")"#), "1073741824");
    assert_eq!(
        text(r#"string.packsize("c536870912c536870912")"#),
        "1073741824"
    );
    for format in [
        "c1073741825",
        "c1000000000000000000000",
        "c536870912c536870913",
        "c1073741824c1",
    ] {
        assert!(
            attempt(&format!(r#"string.packsize("{format}")"#)).contains("format result too large"),
            "packsize {format}"
        );
    }
    assert!(attempt(r#"string.pack("c1073741825", "x")"#).contains("format result too large"));
}

#[test]
fn the_integer_options_keep_their_own_size_limit() {
    assert_eq!(text(r#"string.packsize("i16")"#), "16");
    for format in ["i17", "i20", "i0", "I0", "s20", "i1073741824"] {
        assert!(
            attempt(&format!(r#"string.packsize("{format}")"#))
                .contains("integral size out of limits"),
            "packsize {format}"
        );
    }
}

// 8. An integer wider than 8 bytes only unpacks when the extra bytes carry no information.

#[test]
fn a_wide_integer_must_be_a_sign_extension_of_its_low_eight_bytes() {
    // These used to truncate silently to the low eight bytes.
    for call in [
        r#"string.unpack("<i16", string.rep("\1", 16))"#,
        r#"string.unpack(">i16", "\1" .. string.rep("\0", 15))"#,
        r#"string.unpack("<I16", string.rep("\255", 16))"#,
        r#"string.unpack("<i9", "\1\0\0\0\0\0\0\0\1")"#,
        r#"string.unpack("<s16", string.rep("\1", 16) .. "abc")"#,
    ] {
        let message = attempt(call);
        assert!(
            message.contains("integer does not fit into Lua Integer"),
            "{call} gave {message}"
        );
    }
    assert!(attempt(r#"string.unpack("<i9", "\1\0\0\0\0\0\0\0\1")"#).contains("9-byte integer"));
}

#[test]
fn a_wide_integer_that_does_fit_still_unpacks() {
    let checks = [
        // All-ones is -1 at any width, and its sign extension is the high bytes.
        (r#"string.unpack("<i16", string.rep("\255", 16))"#, "-1"),
        (r#"string.unpack(">i16", string.rep("\255", 16))"#, "-1"),
        (
            r#"string.unpack("<i16", "\5" .. string.rep("\0", 15))"#,
            "5",
        ),
        (
            r#"string.unpack(">i16", string.rep("\0", 15) .. "\5")"#,
            "5",
        ),
        // Unsigned reads only require the unread bytes to be zero, so this is -1 as PUC has it.
        (
            r#"string.unpack("<I16", string.rep("\255", 8) .. string.rep("\0", 8))"#,
            "-1",
        ),
        (r#"string.unpack("<i9", string.rep("\255", 9))"#, "-1"),
        (r#"string.unpack("<i16", string.pack("<i16", -1))"#, "-1"),
        (r#"string.unpack("<i16", string.pack("<i16", 5))"#, "5"),
        (r#"string.unpack(">i16", string.pack(">i16", -7))"#, "-7"),
    ];
    for (expr, want) in checks {
        assert_eq!(text(expr), want, "{expr}");
    }
}

#[test]
fn narrow_integers_are_unchanged_by_the_wide_integer_check() {
    let checks = [
        (r#"string.unpack("<i2", string.pack("<i2", -300))"#, "-300"),
        (r#"string.unpack(">i4", string.pack(">i4", -300))"#, "-300"),
        (r#"string.unpack(">i2", "\255\254")"#, "-2"),
        (r#"string.unpack("<b", string.pack("<b", -1))"#, "-1"),
        (r#"string.unpack("<B", string.pack("<B", 255))"#, "255"),
        (r#"string.unpack("<I1", "\255")"#, "255"),
        (
            r#"string.unpack("<i8", string.pack("<i8", math.mininteger))"#,
            "-9223372036854775808",
        ),
    ];
    for (expr, want) in checks {
        assert_eq!(text(expr), want, "{expr}");
    }
}

// 9. Packing an integer an option cannot hold is an error, not a silent truncation.

#[test]
fn a_signed_option_narrower_than_eight_bytes_rejects_what_it_cannot_hold() {
    // `string.pack("i1", 300)` used to answer "\44" and only show up as corruption wherever the
    // bytes were read back.
    let fits = [
        ("i1", "127"),
        ("i1", "-128"),
        ("i2", "32767"),
        ("i2", "-32768"),
        ("i4", "(1 << 31) - 1"),
        ("i4", "-(1 << 31)"),
        ("i7", "(1 << 55) - 1"),
        ("i7", "-(1 << 55)"),
        ("b", "-128"),
        ("h", "-32768"),
    ];
    for (format, value) in fits {
        assert_eq!(
            text(&format!(r#"#string.pack("<{format}", {value})"#)),
            text(&format!(r#"string.packsize("<{format}")"#)),
            "{format} should accept {value}"
        );
    }
    let overflows = [
        ("i1", "128"),
        ("i1", "-129"),
        ("i1", "300"),
        ("i2", "32768"),
        ("i2", "-32769"),
        ("i4", "1 << 31"),
        ("i4", "-(1 << 31) - 1"),
        ("i7", "1 << 55"),
        ("i7", "-(1 << 55) - 1"),
        ("b", "200"),
        ("h", "40000"),
    ];
    for (format, value) in overflows {
        let message = attempt(&format!(r#"string.pack("<{format}", {value})"#));
        assert!(
            message.contains("integer overflow"),
            "{format} with {value} gave {message}"
        );
    }
}

#[test]
fn an_unsigned_option_takes_its_whole_width_but_no_more() {
    // Unsigned reads the value as unsigned first, so the full width is available and a negative
    // number becomes enormous rather than wrapping.
    for (format, value) in [
        ("I1", "255"),
        ("I2", "65535"),
        ("I4", "(1 << 32) - 1"),
        ("B", "255"),
        ("H", "65535"),
    ] {
        assert_eq!(
            text(&format!(r#"#string.pack("<{format}", {value})"#)),
            text(&format!(r#"string.packsize("<{format}")"#)),
            "{format} should accept {value}"
        );
    }
    for (format, value) in [
        ("I1", "256"),
        ("I1", "-1"),
        ("I2", "65536"),
        ("I2", "-1"),
        ("I4", "1 << 32"),
        ("I7", "1 << 56"),
    ] {
        let message = attempt(&format!(r#"string.pack("<{format}", {value})"#));
        assert!(
            message.contains("unsigned overflow"),
            "{format} with {value} gave {message}"
        );
    }
}

#[test]
fn options_eight_bytes_and_wider_take_any_integer() {
    // Every Lua integer fits, so PUC skips the check entirely at these widths.
    for (format, value) in [
        ("i8", "math.maxinteger"),
        ("i8", "math.mininteger"),
        ("I8", "-1"),
        ("i16", "math.mininteger"),
        ("I16", "-1"),
        ("l", "math.mininteger"),
        ("j", "math.maxinteger"),
        ("T", "-1"),
    ] {
        assert_eq!(
            text(&format!(r#"#string.pack("<{format}", {value})"#)),
            text(&format!(r#"string.packsize("<{format}")"#)),
            "{format} should accept {value}"
        );
    }
}

#[test]
fn an_integer_that_fits_still_round_trips_unchanged() {
    assert_eq!(
        eval::<String>(
            r#"
            local cases = {
                {"<i1", 127}, {"<i1", -128}, {"<I1", 255},
                {"<i2", 32767}, {"<i2", -32768}, {"<I2", 65535},
                {">i2", -32768}, {">I2", 65535},
                {"<i4", -2147483648}, {"<I4", 4294967295},
                {"<i7", (1 << 55) - 1}, {">i7", -(1 << 55)}, {"<I7", (1 << 56) - 1},
                {"<i8", math.mininteger}, {"<i16", math.mininteger},
                {"<b", -128}, {"<B", 255}, {"<h", -32768}, {"<H", 65535},
            }
            for _, c in ipairs(cases) do
                local got = string.unpack(c[1], string.pack(c[1], c[2]))
                if got ~= c[2] then
                    return c[1] .. " gave " .. tostring(got) .. " for " .. tostring(c[2])
                end
            end
            return "all"
        "#
        )
        .unwrap(),
        "all"
    );
}

#[test]
fn the_range_check_keeps_lua_s_own_argument_coercion() {
    // `luaL_checkinteger` accepts a numeric string and an integral float, and the range check
    // applies to whatever they convert to.
    assert_eq!(
        text(r#"string.unpack("<i1", string.pack("<i1", "100"))"#),
        "100"
    );
    assert_eq!(
        text(r#"string.unpack("<i1", string.pack("<i1", 100.0))"#),
        "100"
    );
    assert!(attempt(r#"string.pack("<i1", "300")"#).contains("integer overflow"));
}

#[test]
fn packsize_rejects_the_variable_length_options() {
    // These have no fixed width, so `packsize` cannot answer at all.
    for format in ["z", "s", "s4", "i4z", "i4s1i4"] {
        let message = attempt(&format!(r#"string.packsize("{format}")"#));
        assert!(
            message.contains("variable-length format"),
            "packsize {format} gave {message}"
        );
    }
    assert_eq!(text(r#"string.packsize("i4")"#), "4");
    assert_eq!(text(r#"string.packsize("")"#), "0");
}

#[test]
fn packing_a_float_option_with_a_non_number_is_an_error_not_a_panic() {
    for expr in [
        r#"string.pack("f", "abc")"#,
        r#"string.pack("d", "abc")"#,
        r#"string.pack("f", {})"#,
        r#"string.pack("d", nil)"#,
        r#"string.pack("f")"#,
        r#"string.pack("f", true)"#,
    ] {
        assert!(attempt(expr).contains("number expected"), "{expr}");
    }
    // Values C cannot represent exactly behave as C does rather than failing: `1e300` has no
    // `float`, so it packs as infinity, and arbitrary bytes unpack to NaN.
    assert_eq!(
        text(r#"string.unpack("<f", string.pack("<f", 1e300))"#),
        "inf"
    );
    assert_eq!(
        text(r#"string.unpack("<d", string.pack("<d", math.huge))"#),
        "inf"
    );
    assert_eq!(
        text(r#"string.pack("<f", "1.5") == string.pack("<f", 1.5)"#),
        "true"
    );
    // Arbitrary bytes read back as NaN, which is the one value not equal to itself.
    assert_eq!(
        text(
            r#"(select(1, string.unpack("<d", string.rep("\255", 8)))) ~= string.unpack("<d", string.rep("\255", 8))"#
        ),
        "true"
    );
}

// 10. The string options take strings and numbers, and nothing else.

#[test]
fn a_string_option_rejects_anything_that_is_not_a_string_or_number() {
    // A table used to pack as its own address, so the bytes written into a supposedly stable
    // binary record differed on every run and could never be reproduced.
    for format in ["z", "s1", "s4", "c8"] {
        for (value, want) in [
            ("{}", "string expected, got table"),
            ("true", "string expected, got boolean"),
            ("nil", "string expected, got nil"),
            ("print", "string expected, got function"),
        ] {
            let message = attempt(&format!(r#"string.pack("{format}", {value})"#));
            assert!(
                message.contains(want) && message.contains("#2 to 'pack'"),
                "pack {format} with {value} gave {message}"
            );
        }
        let missing = attempt(&format!(r#"string.pack("{format}")"#));
        assert!(
            missing.contains("string expected, got no value"),
            "pack {format} with nothing gave {missing}"
        );
    }
}

#[test]
fn a_number_still_packs_through_a_string_option_as_its_text() {
    // This is real PUC behaviour, not an oversight: `luaL_checklstring` converts a number in place.
    assert_eq!(text(r#"string.pack("z", 42)"#), "42\0");
    assert_eq!(text(r#"string.pack("z", 2.5)"#), "2.5\0");
    assert_eq!(text(r#"string.pack("z", 2.0)"#), "2.0\0");
    assert_eq!(text(r#"string.unpack("z", string.pack("z", 42))"#), "42");
    assert_eq!(
        text(r#"string.unpack("<s1", string.pack("<s1", 42))"#),
        "42"
    );
    assert_eq!(text(r#"string.unpack("c2", string.pack("c2", 42))"#), "42");
}

#[test]
fn a_zero_terminated_string_may_not_contain_zeros() {
    // `z` is read back up to its first zero, so an embedded one silently drops the rest.
    for value in [r#""a\0b""#, r#""\0""#, r#""ab\0""#] {
        let message = attempt(&format!(r#"string.pack("z", {value})"#));
        assert!(
            message.contains("string contains zeros"),
            "pack z with {value} gave {message}"
        );
    }
    assert_eq!(text(r#"string.unpack("z", string.pack("z", "ab"))"#), "ab");
}

#[test]
fn a_length_prefixed_string_must_fit_its_length_prefix() {
    // The prefix is written at the option's width, so a longer string recorded a truncated count
    // and the payload could never be read back.
    assert_eq!(text(r#"#string.pack("<s1", string.rep("x", 255))"#), "256");
    assert_eq!(
        text(r#"#string.pack("<s2", string.rep("x", 65535))"#),
        "65537"
    );
    for (format, len) in [("s1", 256), ("s1", 300), ("s2", 65536), ("s3", 16777216)] {
        let message = attempt(&format!(
            r#"string.pack("<{format}", string.rep("x", {len}))"#
        ));
        assert!(
            message.contains("string length does not fit in given size"),
            "pack {format} with {len} bytes gave {message}"
        );
    }
    // Eight bytes and wider can hold any length luna allows at all.
    assert_eq!(text(r#"#string.pack("<s8", string.rep("x", 300))"#), "308");
    assert_eq!(
        text(
            r#"string.unpack("<s2", string.pack("<s2", string.rep("x", 65535))) == string.rep("x", 65535)"#
        ),
        "true"
    );
}

// 11. A number with no integer representation is its own mistake, not "number expected".

#[test]
fn a_non_integral_number_is_reported_as_having_no_integer_representation() {
    for expr in [
        r#"string.pack("i1", 1.5)"#,
        r#"string.pack("i8", 1.5)"#,
        r#"string.pack("i1", "1.5")"#,
        r#"string.char(1.5)"#,
        r#"string.char(65, 2.5)"#,
    ] {
        let message = attempt(expr);
        assert!(
            message.contains("number has no integer representation"),
            "{expr} gave {message}"
        );
    }
    // A value that is not a number at all keeps the other message, with its type named.
    assert!(attempt(r#"string.pack("i1", {})"#).contains("number expected, got table"));
    assert!(attempt(r#"string.char({})"#).contains("number expected, got table"));
    assert!(attempt(r#"string.pack("i1")"#).contains("number expected, got no value"));
    assert!(attempt(r#"string.pack("f", {})"#).contains("number expected, got table"));
    // The range check still applies once the value is known to be an integer.
    assert!(attempt(r#"string.char(300)"#).contains("value out of range"));
    assert!(attempt(r#"string.char(-1)"#).contains("value out of range"));
}

#[test]
fn a_float_with_no_fraction_is_still_accepted_where_an_integer_is_wanted() {
    assert_eq!(text(r#"string.char(65.0)"#), "A");
    assert_eq!(
        text(r#"string.unpack("<i1", string.pack("<i1", 2.0))"#),
        "2"
    );
    assert_eq!(text(r#"string.rep("x", 2.0)"#), "xx");
    assert_eq!(text(r#"string.sub("abc", 2.0)"#), "bc");
    assert_eq!(text(r#"string.byte("abc", 2.0)"#), "98");
    assert_eq!(text(r#"("abc"):find("b", 1.0)"#), "2");
    assert_eq!(text(r#"string.unpack("<i1", "\5", 1.0)"#), "5");
    // And a numeric string coerces, as `luaL_checkinteger` does.
    assert_eq!(
        text(r#"string.unpack("<i1", string.pack("<i1", "100"))"#),
        "100"
    );
    assert_eq!(text(r#"string.char("65")"#), "A");
}
