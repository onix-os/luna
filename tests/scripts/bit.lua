function is_err(f)
    return pcall(f) == false
end

-- Strings are NOT bitwise operands: PUC-Rio raises, and so does luna.
function test1()
    return 2   & 3     == 2 and
           2.0 & 3.0   == 2 and
           is_err(function() return "2" & 3.0 end) and
           is_err(function() return 2 & "3.0" end)
end

function test2()
    return 2   | 3     == 3 and
           2.0 | 3.0   == 3 and
           is_err(function() return "2" | 3.0 end) and
           is_err(function() return 2 | "3.0" end)
end

function test3()
    return 2   ~ 3     == 1 and
           2.0 ~ 3.0   == 1 and
           is_err(function() return "2" ~ 3.0 end) and
           is_err(function() return 2 ~ "3.0" end)
end

function test4()
    return ~2   == -3 and
           is_err(function() return ~"2" end) and
           ~2.0 == -3
end

function test5()
    return 2   << 3     == 16 and
           2.0 << 3.0   == 16 and
           is_err(function() return "2" << 3.0 end) and
           is_err(function() return 2 << "3.0" end)
end

function test6()
    return 145   >> 3     == 18 and
           145.0 >> 3.0   == 18 and
           is_err(function() return "145" >> 3.0 end) and
           is_err(function() return 145 >> "3.0" end) and
           -1    >> 1     == 9223372036854775807
end

function test7()
    return is_err(function() return ~2.2    end) and
           is_err(function() return ~"2.2"  end) and
           is_err(function() return 2.2 & 3 end) and
           is_err(function() return 2.2 | 3 end) and
           is_err(function() return 2.2 ~ 3 end) and
           is_err(function() return 2.2 << 3 end) and
           is_err(function() return 2.2 >> 3 end)
end

assert(
    test1() and
    test2() and
    test3() and
    test4() and
    test5() and
    test6() and
    test7()
)
